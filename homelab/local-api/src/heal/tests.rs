use std::collections::HashMap;
use std::sync::Mutex;

use super::*;
use crate::host::fake::FakeHost;

const NOW: u64 = 1_000_000;
const MON_STATUS: &str = "ceph daemon mon.node1 mon_status";

type KubeNodes = Vec<(String, Option<String>)>;

#[derive(Clone)]
struct FakeMachine {
    name: String,
    reset: Option<ResetView>,
    refuses: Option<String>,
}

#[derive(Default)]
struct FakeNetwork {
    machines: Mutex<HashMap<String, FakeMachine>>,
    platform: Option<Vec<PlatformNode>>,
    platform_down: bool,
    kubernetes: Mutex<Option<KubeNodes>>,
    calls: Mutex<Vec<String>>,
}

impl FakeNetwork {
    fn with(machines: &[(&str, &str)]) -> Self {
        let net = Self::default();
        for (name, addr) in machines {
            net.machines.lock().unwrap().insert(
                addr.to_string(),
                FakeMachine {
                    name: name.to_string(),
                    reset: None,
                    refuses: None,
                },
            );
        }
        net
    }

    fn platform(mut self, nodes: &[(i64, &str)]) -> Self {
        self.platform = Some(
            nodes
                .iter()
                .map(|(id, addr)| PlatformNode {
                    node_id: *id,
                    sub_ipv6: addr.to_string(),
                })
                .collect(),
        );
        self
    }

    fn set_phase(&self, addr: &str, heal_id: &str, phase: PhaseView, error: Option<&str>) {
        let mut machines = self.machines.lock().unwrap();
        let m = machines.get_mut(addr).unwrap();
        m.reset = Some(ResetView {
            heal_id: heal_id.into(),
            driver: "node1".into(),
            phase,
            error: error.map(str::to_string),
        });
    }

    fn kubernetes_up(&self, names: &[&str]) {
        *self.kubernetes.lock().unwrap() = Some(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.to_string(), Some(format!("fd00::{}", i + 1))))
                .collect(),
        );
    }

    fn refuse(&self, addr: &str, why: &str) {
        self.machines.lock().unwrap().get_mut(addr).unwrap().refuses = Some(why.into());
    }

    fn disconnect(&self, addr: &str) {
        self.machines.lock().unwrap().remove(addr);
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn answering(&self, addr: &str) -> Result<FakeMachine> {
        self.machines
            .lock()
            .unwrap()
            .get(addr)
            .cloned()
            .context("connection timed out")
    }
}

#[allow(clippy::manual_async_fn)]
impl Network for FakeNetwork {
    fn peer<'a>(&'a self, addr: &'a str) -> impl Future<Output = Result<PeerInfo>> + Send + 'a {
        async move {
            let m = self.answering(addr)?;
            Ok(PeerInfo {
                name: m.name,
                addr: addr.to_string(),
                reset: m.reset,
            })
        }
    }

    fn prepare<'a>(
        &'a self,
        addr: &'a str,
        request: &'a PrepareRequest,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            self.record(format!("prepare {addr} {}", request.server_addr));
            let m = self.answering(addr)?;
            if let Some(why) = m.refuses {
                bail!("{why}");
            }
            self.set_phase(addr, &request.heal_id, PhaseView::Preparing, None);
            Ok(())
        }
    }

    fn arm<'a>(
        &'a self,
        addr: &'a str,
        heal_id: &'a str,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            self.record(format!("arm {addr}"));
            let m = self.answering(addr)?;
            if let Some(why) = m.refuses {
                bail!("{why}");
            }
            self.set_phase(addr, heal_id, PhaseView::Armed, None);
            Ok(())
        }
    }

    fn undo<'a>(
        &'a self,
        addr: &'a str,
        heal_id: &'a str,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            self.record(format!("undo {addr}"));
            self.answering(addr)?;
            self.set_phase(addr, heal_id, PhaseView::Undone, None);
            Ok(())
        }
    }

    fn reboot<'a>(&'a self, addr: &'a str) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            self.record(format!("reboot {addr}"));
            self.answering(addr).map(|_| ())
        }
    }

    fn platform_nodes(
        &self,
    ) -> impl Future<Output = Result<Option<Vec<PlatformNode>>>> + Send + '_ {
        async move {
            if self.platform_down {
                bail!("platform unreachable");
            }
            Ok(self.platform.clone())
        }
    }

    fn delete_platform_node(&self, id: i64) -> impl Future<Output = Result<()>> + Send + '_ {
        async move {
            self.record(format!("delete platform node {id}"));
            Ok(())
        }
    }

    fn kubernetes_nodes(&self) -> impl Future<Output = Result<Option<KubeNodes>>> + Send + '_ {
        async move { Ok(self.kubernetes.lock().unwrap().clone()) }
    }
}

fn mon_status_json(state: &str, mons: &[(&str, &str)]) -> String {
    json!({
        "name": "node1",
        "state": state,
        "monmap": {"mons": mons.iter().map(|(n, addr)| json!({
            "name": n,
            "public_addrs": {"addrvec": [
                {"type": "v2", "addr": format!("[{addr}]:3300"), "nonce": 0},
                {"type": "v1", "addr": format!("[{addr}]:6789"), "nonce": 0},
            ]},
        })).collect::<Vec<_>>()},
    })
    .to_string()
}

fn local() -> (tempfile::TempDir, LocalRecord) {
    let dir = tempfile::tempdir().unwrap();
    let record = LocalRecord::under(dir.path());
    (dir, record)
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn broken_cluster() -> (FakeHost, FakeNetwork) {
    let host = FakeHost::new().ok(
        MON_STATUS,
        &mon_status_json(
            "probing",
            &[
                ("node1", "fd00::1"),
                ("node2", "fd00::2"),
                ("node3", "fd00::3"),
            ],
        ),
    );
    let net = FakeNetwork::with(&[("node1", "fd00::1"), ("node3", "fd00::3")]).platform(&[
        (11, "fd00::1"),
        (12, "fd00::2"),
        (13, "fd00::3"),
    ]);
    (host, net)
}

fn request() -> HealRequest {
    HealRequest {
        keep_machines: set(&["node1", "node3"]),
        remove_machines: set(&["node2"]),
    }
}

async fn started(host: &FakeHost, net: &FakeNetwork, record: &LocalRecord) -> Heal {
    start_heal(
        host,
        net,
        record,
        "node1",
        "fd00::1",
        &request(),
        5_000,
        "ab12".into(),
        NOW,
    )
    .await
    .unwrap()
}

#[test]
fn mon_status_gives_quorum_and_every_mon_with_its_address() {
    let s = parse_mon_status(&mon_status_json(
        "peon",
        &[("node1", "fd00::1"), ("node2", "fd00::2")],
    ))
    .unwrap();
    assert!(s.in_quorum);
    assert_eq!(
        s.mons,
        vec![
            ("node1".into(), "fd00::1".into()),
            ("node2".into(), "fd00::2".into())
        ]
    );
    assert!(
        !parse_mon_status(&mon_status_json("probing", &[]))
            .unwrap()
            .in_quorum
    );
    assert!(
        !parse_mon_status(&mon_status_json("electing", &[]))
            .unwrap()
            .in_quorum
    );
    assert!(parse_mon_status("{}").is_err());
    assert!(parse_mon_status(r#"{"state":"leader","monmap":{"mons":[{"name":"x"}]}}"#).is_err());
}

#[test]
fn addresses_from_different_lists_are_compared_as_addresses() {
    assert_eq!(host_of("[fd00::1]:3300").as_deref(), Some("fd00::1"));
    assert_eq!(host_of("nope"), None);
    assert_eq!(normalize("fd00:0:0::1/128"), "fd00::1");
    assert_eq!(normalize("fd00::1"), "fd00::1");
}

#[test]
fn a_new_fsid_is_a_version_4_uuid() {
    let a = new_fsid();
    assert_eq!(a.len(), 36);
    assert_eq!(&a[14..15], "4");
    assert!(member::rewrite_config("[node.k3s]\n", "", &a).is_ok());
    assert_ne!(a, new_fsid());
}

#[tokio::test]
async fn every_listed_machine_is_asked_and_the_silent_ones_are_left_behind() {
    let (host, mut net) = broken_cluster();
    net.platform.as_mut().unwrap().push(PlatformNode {
        node_id: 14,
        sub_ipv6: "fd00:0::4".into(),
    });

    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;

    let kept: Vec<String> = s.kept().map(MachineState::label).collect();
    let gone: Vec<(String, Option<i64>)> = s.gone().map(|m| (m.label(), m.platform_id)).collect();
    assert_eq!(kept, ["node1", "node3"]);
    assert_eq!(
        gone,
        [
            ("node2".to_string(), Some(12)),
            ("fd00::4".to_string(), Some(14))
        ]
    );
    assert!(
        s.machines
            .iter()
            .find(|m| m.addr == "fd00::1")
            .unwrap()
            .this_machine
    );
    assert_eq!(
        s.problems(),
        ["machines_gone", "ceph_no_quorum", "kubernetes_down"]
    );
    assert_eq!(s.refusal(None), None);
}

#[tokio::test]
async fn with_the_platform_down_the_monmap_still_lists_the_machines() {
    let (host, mut net) = broken_cluster();
    net.platform_down = true;
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert_eq!(
        s.gone().map(MachineState::label).collect::<Vec<_>>(),
        ["node2"]
    );
    assert!(s.unreadable[0].contains("platform"));
    assert_eq!(s.refusal(None), None);
}

#[tokio::test]
async fn a_heal_is_refused_when_no_list_of_machines_can_be_read() {
    let host = FakeHost::new().fail(MON_STATUS, "admin socket not found");
    let mut net = FakeNetwork::with(&[("node1", "fd00::1")]);
    net.platform_down = true;
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert!(s.refusal(None).unwrap().contains("no list"));
}

#[tokio::test]
async fn a_healthy_cluster_has_nothing_to_heal() {
    let host = FakeHost::new()
        .ok(
            MON_STATUS,
            &mon_status_json("leader", &[("node1", "fd00::1"), ("node2", "fd00::2")]),
        )
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":0,"up":1,"in":1}],"pools":[]}"#,
        )
        .ok("ceph pg dump pgs_brief", "[]");
    let net = FakeNetwork::with(&[("node1", "fd00::1"), ("node2", "fd00::2")]);
    net.kubernetes_up(&["node1", "node2"]);
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert!(s.problems().is_empty(), "{:?}", s.problems());
    assert!(s.refusal(None).unwrap().contains("nothing is wrong"));
}

#[tokio::test]
async fn a_heal_another_answering_machine_drives_is_not_started_over() {
    let (host, net) = broken_cluster();
    net.set_phase("fd00::3", "ffff", PhaseView::Preparing, None);
    net.machines
        .lock()
        .unwrap()
        .get_mut("fd00::3")
        .unwrap()
        .reset
        .as_mut()
        .unwrap()
        .driver = "node3".into();
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert!(s
        .refusal(None)
        .unwrap()
        .contains("node3 is already healing"));

    net.machines
        .lock()
        .unwrap()
        .get_mut("fd00::3")
        .unwrap()
        .reset
        .as_mut()
        .unwrap()
        .driver = "node2".into();
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert_eq!(s.refusal(None), None);
}

#[tokio::test]
async fn a_heal_starts_only_on_what_the_owner_saw() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    let stale = HealRequest {
        keep_machines: set(&["node1"]),
        remove_machines: set(&["node2", "node3"]),
    };
    let err = start_heal(
        &host,
        &net,
        &record,
        "node1",
        "fd00::1",
        &stale,
        5_000,
        "ab12".into(),
        NOW,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("changed since the page was loaded"),
        "{err}"
    );
    assert!(record.load().unwrap().is_none());

    let heal = started(&host, &net, &record).await;
    assert_eq!(heal.step, Step::Prepare);
    assert_eq!(
        heal.members,
        vec![
            Member {
                name: "node3".into(),
                addr: "fd00::3".into()
            },
            Member {
                name: "node1".into(),
                addr: "fd00::1".into()
            },
        ],
        "the driver last"
    );
    assert_eq!(heal.gone[0].platform_id, Some(12));
    assert_eq!(record.load().unwrap(), Some(heal.clone()));

    let err = start_heal(
        &host,
        &net,
        &record,
        "node1",
        "fd00::1",
        &request(),
        5_000,
        "cd34".into(),
        NOW,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("already healing"), "{err}");
}

#[test]
fn the_driver_creates_the_cluster_and_every_other_machine_joins_it() {
    let heal = Heal {
        id: "ab12".into(),
        driver: "node1".into(),
        started_at: NOW,
        finished_at: None,
        step: Step::Prepare,
        fsid: new_fsid(),
        members: vec![
            Member {
                name: "node3".into(),
                addr: "fd00::3".into(),
            },
            Member {
                name: "node1".into(),
                addr: "fd00::1".into(),
            },
        ],
        gone: vec![],
        restart_boot_id: None,
        waiting: None,
        failed: None,
    };
    assert_eq!(heal.request_for(&heal.members[1]).unwrap().server_addr, "");
    assert_eq!(
        heal.request_for(&heal.members[0]).unwrap().server_addr,
        "https://[fd00::1]:6443"
    );
}

#[tokio::test]
async fn a_heal_prepares_everywhere_arms_restarts_all_and_waits_for_the_new_cluster() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    started(&host, &net, &record).await;
    let host = host.ok("systemctl reboot", "");

    tick(&host, &net, &record, "boot1", NOW + 10).await.unwrap();
    let calls = net.calls();
    assert!(
        calls.contains(&"prepare fd00::3 https://[fd00::1]:6443".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"prepare fd00::1 ".to_string()), "{calls:?}");
    assert!(
        !calls.iter().any(|c| c.starts_with("arm")),
        "nothing is armed before all are prepared"
    );
    let heal = record.load().unwrap().unwrap();
    assert_eq!(heal.step, Step::Prepare);
    assert!(heal.waiting.unwrap().contains("preparing"));

    net.set_phase("fd00::3", "ab12", PhaseView::Prepared, None);
    tick(&host, &net, &record, "boot1", NOW + 15).await.unwrap();
    assert!(!net.calls().iter().any(|c| c.starts_with("arm")));

    net.set_phase("fd00::1", "ab12", PhaseView::Prepared, None);
    tick(&host, &net, &record, "boot1", NOW + 20).await.unwrap();
    let calls = net.calls();
    let at = |c: &str| calls.iter().position(|x| x == c).unwrap();
    assert!(at("arm fd00::3") < at("arm fd00::1"));
    assert!(at("arm fd00::1") < at("reboot fd00::3"));
    assert!(
        !calls.contains(&"reboot fd00::1".to_string()),
        "this machine restarts itself"
    );
    assert!(host.ran("systemctl reboot"));
    let heal = record.load().unwrap().unwrap();
    assert_eq!(heal.step, Step::Rebuild);
    assert_eq!(heal.restart_boot_id.as_deref(), Some("boot1"));

    net.set_phase("fd00::3", "ab12", PhaseView::Restarted, None);
    let host = FakeHost::new();
    tick(&host, &net, &record, "boot2", NOW + 300)
        .await
        .unwrap();
    assert!(record
        .load()
        .unwrap()
        .unwrap()
        .waiting
        .unwrap()
        .contains("Kubernetes is starting"));

    net.kubernetes_up(&["node1"]);
    tick(&host, &net, &record, "boot2", NOW + 400)
        .await
        .unwrap();
    let heal = record.load().unwrap().unwrap();
    assert!(heal.running());
    assert!(heal.waiting.unwrap().contains("node3 has not joined"));
    assert!(!net
        .calls()
        .iter()
        .any(|c| c.starts_with("delete platform node")));

    net.kubernetes_up(&["node1", "node3"]);
    tick(&host, &net, &record, "boot2", NOW + 500)
        .await
        .unwrap();
    let heal = record.load().unwrap().unwrap();
    assert_eq!(heal.finished_at, Some(NOW + 500));
    assert_eq!(heal.failed, None);
    assert!(net.calls().contains(&"delete platform node 12".to_string()));
    assert!(!net.calls().iter().any(|c| c.starts_with("undo")));
}

#[tokio::test]
async fn a_restart_that_never_happened_is_tried_again() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    let mut heal = started(&host, &net, &record).await;
    heal.step = Step::Rebuild;
    heal.restart_boot_id = Some("boot1".into());
    record.save(&heal).unwrap();
    let host = FakeHost::new().ok("systemctl reboot", "");
    tick(&host, &net, &record, "boot1", NOW + 30).await.unwrap();
    assert!(host.ran("systemctl reboot"));
}

#[tokio::test]
async fn a_machine_that_did_not_restart_is_asked_again() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    let mut heal = started(&host, &net, &record).await;
    heal.step = Step::Rebuild;
    heal.restart_boot_id = Some("boot1".into());
    record.save(&heal).unwrap();
    net.set_phase("fd00::3", "ab12", PhaseView::Armed, None);
    net.kubernetes_up(&["node1"]);
    let host = FakeHost::new();
    tick(&host, &net, &record, "boot2", NOW + 30).await.unwrap();
    assert!(net.calls().contains(&"reboot fd00::3".to_string()));
    assert!(record
        .load()
        .unwrap()
        .unwrap()
        .waiting
        .unwrap()
        .contains("node3 has not restarted"));
}

#[tokio::test]
async fn a_failed_prepare_undoes_the_heal_on_every_machine() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    started(&host, &net, &record).await;
    net.set_phase("fd00::1", "ab12", PhaseView::Prepared, None);
    net.set_phase(
        "fd00::3",
        "ab12",
        PhaseView::Failed,
        Some("no space left on device"),
    );

    tick(&host, &net, &record, "boot1", NOW + 10).await.unwrap();

    let heal = record.load().unwrap().unwrap();
    assert!(heal.failed.as_deref().unwrap().contains("no space left"));
    assert_eq!(heal.finished_at, Some(NOW + 10));
    let calls = net.calls();
    assert!(
        calls.contains(&"undo fd00::3".to_string()) && calls.contains(&"undo fd00::1".to_string())
    );
    assert!(!calls
        .iter()
        .any(|c| c.starts_with("arm") || c.starts_with("reboot")));
    assert!(!host.ran("systemctl reboot"));
}

#[tokio::test]
async fn a_machine_that_cannot_be_armed_undoes_the_ones_that_were() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    started(&host, &net, &record).await;
    net.set_phase("fd00::3", "ab12", PhaseView::Prepared, None);
    net.set_phase("fd00::1", "ab12", PhaseView::Prepared, None);
    net.refuse("fd00::1", "read-only file system");
    let mut heal = record.load().unwrap().unwrap();
    heal.step = Step::Arm;
    record.save(&heal).unwrap();

    tick(&host, &net, &record, "boot1", NOW + 10).await.unwrap();

    let heal = record.load().unwrap().unwrap();
    assert!(heal
        .failed
        .as_deref()
        .unwrap()
        .contains("node1 could not be armed"));
    let calls = net.calls();
    assert!(calls.contains(&"arm fd00::3".to_string()));
    assert!(calls.contains(&"undo fd00::3".to_string()));
    assert!(!calls.iter().any(|c| c.starts_with("reboot")));
}

#[tokio::test]
async fn an_undo_keeps_trying_a_machine_that_does_not_answer() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    let mut heal = started(&host, &net, &record).await;
    heal.step = Step::Undo;
    heal.failed = Some("node3 could not be armed".into());
    record.save(&heal).unwrap();
    net.disconnect("fd00::3");

    tick(&host, &net, &record, "boot1", NOW + 10).await.unwrap();
    let heal = record.load().unwrap().unwrap();
    assert!(heal.running());
    assert!(heal.waiting.unwrap().contains("node3"));

    assert!(Step::Undo.replaceable() && Step::Rebuild.replaceable());
    assert!(!Step::Arm.replaceable() && !Step::Prepare.replaceable());
}

#[tokio::test]
async fn a_prepare_that_never_finishes_is_given_up() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    started(&host, &net, &record).await;
    net.set_phase("fd00::1", "ab12", PhaseView::Prepared, None);
    net.set_phase("fd00::3", "ab12", PhaseView::Preparing, None);
    tick(&host, &net, &record, "boot1", NOW + PREPARE_WAIT_SECS)
        .await
        .unwrap();
    let heal = record.load().unwrap().unwrap();
    assert!(heal.failed.unwrap().contains("did not finish preparing"));
}

#[test]
fn backups_stop_only_when_app_data_is_lost() {
    let mut lost = PgsByPool::new();
    lost.insert("yolab-images".into(), BTreeSet::from(["2.1".to_string()]));
    assert_eq!(blocked_by_loss(&lost), None);
    lost.insert("yolab-fs-data0".into(), BTreeSet::from(["3.1".to_string()]));
    assert!(blocked_by_loss(&lost).is_some());
}

mod real_network {
    use super::*;
    use crate::testkit::{peer, PEER};
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "cluster-tok";

    fn network(port: u16, platform_url: &str, kube: kube::Client) -> RealNetwork {
        RealNetwork {
            kube: crate::k8s::Kube::with(kube),
            client: crate::testkit::http(),
            port,
            token: TOKEN.into(),
            platform_url: platform_url.into(),
        }
    }

    fn to_peer(port: u16) -> RealNetwork {
        network(port, "", crate::k8s::testing::unreachable())
    }

    async fn answering(verb: &str, route: &str, status: u16, body: Value) -> (MockServer, u16) {
        let (server, port) = peer().await;
        Mock::given(matchers::method(verb))
            .and(matchers::path(route))
            .and(matchers::header(crate::auth::CLUSTER_AUTH_HEADER, TOKEN))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        (server, port)
    }

    #[tokio::test]
    async fn a_peer_is_asked_who_it_is_with_the_cluster_token() {
        let (_server, port) = answering(
            "GET",
            "/api/heal/peer",
            200,
            json!({ "name": "node2", "addr": "fd00:cafe::6", "reset": null }),
        )
        .await;
        let info = to_peer(port).peer(PEER).await.unwrap();
        assert_eq!(info.name, "node2");
        assert_eq!(info.addr, "fd00:cafe::6");
        assert!(info.reset.is_none());
    }

    #[tokio::test]
    async fn a_peer_is_sent_the_whole_prepare_request() {
        let request = PrepareRequest {
            heal_id: "abc123".into(),
            driver: "node1".into(),
            fsid: "11111111-2222-3333-4444-555555555555".into(),
            server_addr: "https://[fd00:cafe::5]:6443".into(),
        };
        let (server, port) = answering("POST", "/api/heal/peer/prepare", 200, json!({})).await;
        to_peer(port).prepare(PEER, &request).await.unwrap();
        let sent: PrepareRequest = server.received_requests().await.unwrap()[0]
            .body_json()
            .unwrap();
        assert_eq!(sent, request);
    }

    #[tokio::test]
    async fn arming_and_undoing_name_the_heal() {
        let (arm, port) = answering("POST", "/api/heal/peer/arm", 200, json!({})).await;
        to_peer(port).arm(PEER, "abc123").await.unwrap();
        let sent: Value = arm.received_requests().await.unwrap()[0]
            .body_json()
            .unwrap();
        assert_eq!(sent, json!({ "heal_id": "abc123" }));

        let (undo, port) = answering("POST", "/api/heal/peer/undo", 200, json!({})).await;
        to_peer(port).undo(PEER, "abc123").await.unwrap();
        let sent: Value = undo.received_requests().await.unwrap()[0]
            .body_json()
            .unwrap();
        assert_eq!(sent, json!({ "heal_id": "abc123" }));
    }

    #[tokio::test]
    async fn a_peer_is_rebooted_through_its_own_api() {
        let (_server, port) = answering("POST", "/api/system/reboot", 200, json!({})).await;
        to_peer(port).reboot(PEER).await.unwrap();
    }

    #[tokio::test]
    async fn a_refusal_carries_the_peers_own_reason() {
        let (_server, port) = answering(
            "POST",
            "/api/heal/peer/arm",
            409,
            json!({ "error": "this machine was not prepared for heal abc123" }),
        )
        .await;
        let err = to_peer(port).arm(PEER, "abc123").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("409"), "{msg}");
        assert!(msg.contains("was not prepared for heal abc123"), "{msg}");
    }

    #[tokio::test]
    async fn without_a_platform_or_a_token_there_are_no_platform_nodes() {
        let server = MockServer::start().await;
        let unknown = network(1, "", crate::k8s::testing::unreachable());
        assert_eq!(unknown.platform_nodes().await.unwrap(), None);
        let mut anonymous = network(1, &server.uri(), crate::k8s::testing::unreachable());
        anonymous.token = String::new();
        assert_eq!(anonymous.platform_nodes().await.unwrap(), None);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn platform_nodes_are_listed_with_the_account_token() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/nodes"))
            .and(matchers::header("authorization", "Bearer cluster-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "node_id": 7, "sub_ipv6": "fd00:cafe::6", "extra": "ignored" }
            ])))
            .mount(&server)
            .await;
        let nodes = network(1, &server.uri(), crate::k8s::testing::unreachable())
            .platform_nodes()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            nodes,
            [PlatformNode {
                node_id: 7,
                sub_ipv6: "fd00:cafe::6".into()
            }]
        );
    }

    #[tokio::test]
    async fn deleting_a_platform_node_that_is_already_gone_is_success() {
        let server = MockServer::start().await;
        Mock::given(matchers::method("DELETE"))
            .and(matchers::path("/nodes/7"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(matchers::method("DELETE"))
            .and(matchers::path("/nodes/8"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let net = network(1, &server.uri(), crate::k8s::testing::unreachable());
        net.delete_platform_node(7).await.unwrap();
        assert!(net.delete_platform_node(8).await.is_err());
    }

    #[tokio::test]
    async fn without_a_reachable_kubernetes_there_are_no_kubernetes_nodes() {
        assert_eq!(to_peer(1).kubernetes_nodes().await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_api_server_that_is_not_ready_is_not_asked_for_nodes() {
        let (server, kube) = crate::k8s::testing::api_server().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/readyz"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert_eq!(network(1, "", kube).kubernetes_nodes().await.unwrap(), None);
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/readyz"));
    }
}
