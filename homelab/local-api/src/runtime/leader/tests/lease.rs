use super::*;
use crate::k8s::testing::{api_server, status};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ITEM: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/yolab-leader";
const COLLECTION: &str = "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases";

fn held_by(holder: &str, version: Option<&str>) -> Value {
    let mut metadata = json!({ "name": LEASE_NAME, "namespace": LEASE_NS });
    if let Some(v) = version {
        metadata["resourceVersion"] = json!(v);
    }
    json!({
        "apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease",
        "metadata": metadata,
        "spec": {
            "holderIdentity": holder,
            "leaseDurationSeconds": 30,
            "renewTime": Utc::now().to_rfc3339(),
        },
    })
}

fn answer(code: u16) -> ResponseTemplate {
    if code < 300 {
        ResponseTemplate::new(code).set_body_json(held_by("node1", Some("43")))
    } else {
        ResponseTemplate::new(code).set_body_json(status(code, "Refused"))
    }
}

async fn lease_is(server: &MockServer, lease: Option<Value>) {
    let found = match lease {
        Some(l) => ResponseTemplate::new(200).set_body_json(l),
        None => ResponseTemplate::new(404).set_body_json(status(404, "NotFound")),
    };
    Mock::given(method("GET"))
        .and(path(ITEM))
        .respond_with(found)
        .mount(server)
        .await;
}

async fn attempt(lease: Option<Value>, verb: &str, code: u16) -> anyhow::Result<bool> {
    let (server, kube) = api_server().await;
    lease_is(&server, lease).await;
    let at = if verb == "POST" { COLLECTION } else { ITEM };
    Mock::given(method(verb))
        .and(path(at))
        .respond_with(answer(code))
        .mount(&server)
        .await;
    try_acquire(&kube, "node1", Utc::now(), &mut None).await
}

#[tokio::test]
async fn a_missing_lease_is_created_unless_another_machine_got_there_first() {
    assert!(attempt(None, "POST", 201).await.unwrap());
    assert!(!attempt(None, "POST", 409).await.unwrap());
    assert!(attempt(None, "POST", 500).await.is_err());
}

#[tokio::test]
async fn our_own_lease_is_renewed_unless_it_changed_under_us() {
    let ours = || Some(held_by("node1", Some("42")));
    assert!(attempt(ours(), "PUT", 200).await.unwrap());
    assert!(!attempt(ours(), "PUT", 409).await.unwrap());
    assert!(attempt(ours(), "PUT", 500).await.is_err());
}

#[tokio::test]
async fn a_live_lease_of_another_machine_is_left_to_it() {
    assert!(!attempt(Some(held_by("node2", Some("42"))), "PUT", 200)
        .await
        .unwrap());
}

#[tokio::test]
async fn a_lease_without_a_version_cannot_be_renewed_safely() {
    assert_eq!(
        attempt(Some(held_by("node1", None)), "PUT", 200)
            .await
            .unwrap_err()
            .to_string(),
        "the lease has no resourceVersion"
    );
}

#[tokio::test]
async fn a_machine_takes_the_lease_then_stands_down_when_another_holds_it() {
    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path(ITEM))
        .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    lease_is(&server, Some(held_by("node2", Some("50")))).await;
    Mock::given(method("POST"))
        .and(path(COLLECTION))
        .respond_with(answer(201))
        .expect(1)
        .mount(&server)
        .await;
    let leadership = start("node1".into(), Kube::with(kube));
    tokio::time::sleep(RENEW_EVERY * 8).await;
    assert!(!leadership.is_leader());
    assert!(!current_holder_is_me());
    let reads = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "GET")
        .count();
    assert!(reads >= 3, "{reads}");
}

#[tokio::test]
async fn a_machine_without_a_cluster_keeps_trying_and_never_leads() {
    let leadership = start("node1".into(), Kube::from_environment());
    tokio::time::sleep(RENEW_EVERY * 4).await;
    assert!(!leadership.is_leader());
}

#[tokio::test]
async fn a_machine_without_a_name_never_stands() {
    assert!(!start(String::new(), Kube::from_environment()).is_leader());
}

#[tokio::test]
async fn a_lease_that_cannot_be_read_is_an_error_not_a_free_lease() {
    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path(ITEM))
        .respond_with(answer(500))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(COLLECTION))
        .respond_with(answer(201))
        .expect(0)
        .mount(&server)
        .await;
    assert!(try_acquire(&kube, "node1", Utc::now(), &mut None)
        .await
        .is_err());
}

#[tokio::test]
async fn a_machine_that_keeps_renewing_its_own_lease_keeps_leading() {
    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path(ITEM))
        .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    lease_is(&server, Some(held_by("node1", Some("43")))).await;
    Mock::given(method("POST"))
        .and(path(COLLECTION))
        .respond_with(answer(201))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(ITEM))
        .respond_with(answer(200))
        .mount(&server)
        .await;
    let leadership = start("node1".into(), Kube::with(kube));
    tokio::time::sleep(RENEW_EVERY * 4).await;
    assert!(leadership.is_leader());
    let renewals = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .count();
    assert!(renewals >= 1, "{renewals}");
}
