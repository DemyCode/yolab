use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::error::Outcome;
use crate::host::Host;

use super::settings;

pub const KEY: &str = "yolab/containerd-pivot";
const HOLD: Duration = Duration::from_secs(90 * 60);
const SETTLE: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Holder {
    node: String,
    until: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    Held,
    Busy(String),
}

fn busy(holder: &Holder) -> Claim {
    Claim::Busy(format!(
        "{} is swapping its image store — machines take turns, since stopping k3s on one \
         stalls the others",
        holder.node
    ))
}

pub async fn claim<H: Host>(host: &H, node: &str, now: u64) -> Result<Claim> {
    if let Some(holder) = settings::get_json::<H, Holder>(host, KEY).await? {
        if holder.node != node && holder.until > now {
            return Ok(busy(&holder));
        }
    }
    let mine = Holder {
        node: node.to_string(),
        until: now + HOLD.as_secs(),
    };
    settings::set_json(host, KEY, &mine).await?;
    tokio::time::sleep(SETTLE).await;
    Ok(match settings::get_json::<H, Holder>(host, KEY).await? {
        Some(holder) if holder.node == node => Claim::Held,
        Some(holder) => busy(&holder),
        None => Claim::Busy("the claim on the image-store swap disappeared".into()),
    })
}

pub async fn release<H: Host>(host: &H, node: &str) {
    match settings::get_json::<H, Holder>(host, KEY).await {
        Ok(Some(holder)) if holder.node == node => {
            host.ceph(&["config-key", "rm", KEY])
                .await
                .warn_on_err("release the image-store swap claim");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("image-store swap claim unreadable, left to expire: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const NOW: u64 = 1_000_000;
    const GET: &str = "ceph config-key get yolab/containerd-pivot";

    fn held_by(node: &str, until: u64) -> String {
        serde_json::to_string(&Holder {
            node: node.into(),
            until,
        })
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_free_swap_is_claimed_and_confirmed() {
        let host = FakeHost::new()
            .fail(GET, "Error ENOENT: no such key")
            .ok(GET, &held_by("n1", NOW + HOLD.as_secs()))
            .ok("ceph config-key set", "");
        assert_eq!(claim(&host, "n1", NOW).await.unwrap(), Claim::Held);
        assert!(host.ran(&format!(
            "ceph config-key set yolab/containerd-pivot {}",
            held_by("n1", NOW + HOLD.as_secs())
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn another_machine_mid_swap_makes_this_one_wait_without_writing() {
        let host = FakeHost::new().ok(GET, &held_by("n2", NOW + 60));
        match claim(&host, "n1", NOW).await.unwrap() {
            Claim::Busy(why) => assert!(why.contains("n2 is swapping")),
            Claim::Held => panic!("two machines swapped at once"),
        }
        assert!(!host.ran("config-key set"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_left_by_a_machine_that_died_expires() {
        let host = FakeHost::new()
            .ok(GET, &held_by("n2", NOW - 1))
            .ok(GET, &held_by("n1", NOW + HOLD.as_secs()))
            .ok("ceph config-key set", "");
        assert_eq!(claim(&host, "n1", NOW).await.unwrap(), Claim::Held);
    }

    #[tokio::test(start_paused = true)]
    async fn a_machine_that_restarted_mid_swap_takes_its_own_claim_back() {
        let host = FakeHost::new()
            .ok(GET, &held_by("n1", NOW + 60))
            .ok(GET, &held_by("n1", NOW + HOLD.as_secs()))
            .ok("ceph config-key set", "");
        assert_eq!(claim(&host, "n1", NOW).await.unwrap(), Claim::Held);
    }

    #[tokio::test(start_paused = true)]
    async fn losing_a_simultaneous_claim_backs_off() {
        let host = FakeHost::new()
            .fail(GET, "Error ENOENT: no such key")
            .ok(GET, &held_by("n2", NOW + HOLD.as_secs()))
            .ok("ceph config-key set", "");
        assert!(matches!(
            claim(&host, "n1", NOW).await.unwrap(),
            Claim::Busy(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cluster_that_cannot_answer_is_an_error_not_a_free_swap() {
        let host = FakeHost::new().fail(GET, "error connecting to the cluster");
        assert!(claim(&host, "n1", NOW).await.is_err());
        assert!(!host.ran("config-key set"));
    }

    #[tokio::test]
    async fn only_the_holder_releases_the_claim() {
        let mine = FakeHost::new()
            .ok(GET, &held_by("n1", NOW))
            .ok("ceph config-key rm", "");
        release(&mine, "n1").await;
        assert!(mine.ran("ceph config-key rm yolab/containerd-pivot"));

        let theirs = FakeHost::new().ok(GET, &held_by("n2", NOW));
        release(&theirs, "n1").await;
        assert!(!theirs.ran("config-key rm"));
    }
}
