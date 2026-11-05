use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::http::{Client, StatusCode};
use crate::notify::Tunnel;

const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Volume {
    pub id: i64,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    pub box_type: String,
    pub size_gb: u64,
    #[serde(default)]
    pub status: String,
}

impl Volume {
    pub fn is_ready(&self) -> bool {
        self.status == "active"
            && !self.host.is_empty()
            && !self.username.is_empty()
            && !self.password.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Offer {
    pub box_type: String,
    pub size_gb: u64,
    pub monthly_cents: u64,
}

fn connected(tunnel: &Tunnel) -> Result<()> {
    if tunnel.platform_api_url.is_empty() || tunnel.account_token.is_empty() {
        bail!("this machine is not connected to the YoLab platform, which provides virtual disks");
    }
    Ok(())
}

fn url(tunnel: &Tunnel, path: &str) -> String {
    format!("{}/storage/volume{path}", tunnel.platform_api_url)
}

async fn refused(resp: crate::http::Response, what: &str) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::anyhow!("the platform refused to {what} ({status}): {}", body.trim())
}

pub(crate) async fn create(client: &Client, tunnel: &Tunnel, box_type: &str) -> Result<Volume> {
    connected(tunnel)?;
    let resp = client
        .post(url(tunnel, ""))
        .bearer_auth(&tunnel.account_token)
        .timeout(TIMEOUT)
        .json(&serde_json::json!({ "box_type": box_type }))
        .send()
        .await
        .context("reach the platform to buy a virtual disk")?;
    if !resp.status().is_success() {
        return Err(refused(resp, "create a virtual disk").await);
    }
    resp.json::<Volume>()
        .await
        .context("read the platform's answer for the new virtual disk")
}

pub(crate) async fn list(client: &Client, tunnel: &Tunnel) -> Result<Vec<Volume>> {
    connected(tunnel)?;
    let resp = client
        .get(url(tunnel, ""))
        .bearer_auth(&tunnel.account_token)
        .timeout(TIMEOUT)
        .send()
        .await
        .context("reach the platform to list virtual disks")?;
    if !resp.status().is_success() {
        return Err(refused(resp, "list virtual disks").await);
    }
    resp.json::<Vec<Volume>>()
        .await
        .context("read the platform's list of virtual disks")
}

pub(crate) async fn offers(client: &Client, tunnel: &Tunnel) -> Result<Vec<Offer>> {
    connected(tunnel)?;
    let resp = client
        .get(url(tunnel, "/offers"))
        .bearer_auth(&tunnel.account_token)
        .timeout(TIMEOUT)
        .send()
        .await
        .context("reach the platform for virtual disk prices")?;
    if !resp.status().is_success() {
        return Err(refused(resp, "give virtual disk prices").await);
    }
    resp.json::<Vec<Offer>>()
        .await
        .context("read the platform's virtual disk prices")
}

pub(crate) async fn delete(client: &Client, tunnel: &Tunnel, id: i64) -> Result<()> {
    connected(tunnel)?;
    let resp = client
        .delete(url(tunnel, &format!("/{id}")))
        .bearer_auth(&tunnel.account_token)
        .timeout(TIMEOUT)
        .send()
        .await
        .context("reach the platform to delete a virtual disk")?;
    if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
        return Ok(());
    }
    Err(refused(resp, "delete a virtual disk").await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tunnel(server: &MockServer) -> Tunnel {
        Tunnel {
            enabled: true,
            platform_api_url: server.uri(),
            account_token: "tok".into(),
            tunnel_id: "t".into(),
            host: String::new(),
        }
    }

    #[tokio::test]
    async fn a_virtual_disk_is_bought_with_the_account_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/storage/volume"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 3, "host": "", "port": 445, "username": "", "password": "pw",
                "box_type": "bx11", "size_gb": 900, "status": "initializing",
                "created_at": "2026-11-04T00:00:00Z"
            })))
            .mount(&server)
            .await;
        let v = create(&Client::new(), &tunnel(&server), "bx11")
            .await
            .unwrap();
        assert_eq!(v.id, 3);
        assert!(!v.is_ready());
    }

    #[tokio::test]
    async fn prices_come_from_the_platform_as_it_sends_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/storage/volume/offers"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "box_type": "bx11", "size_gb": 900, "monthly_cents": 640 }
            ])))
            .mount(&server)
            .await;
        let got = offers(&Client::new(), &tunnel(&server)).await.unwrap();
        assert_eq!(
            got,
            vec![Offer {
                box_type: "bx11".into(),
                size_gb: 900,
                monthly_cents: 640
            }]
        );
    }

    #[tokio::test]
    async fn deleting_a_box_the_platform_no_longer_has_counts_as_done() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/storage/volume/3"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        delete(&Client::new(), &tunnel(&server), 3).await.unwrap();
    }

    #[tokio::test]
    async fn a_platform_error_is_reported_not_swallowed() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/storage/volume/3"))
            .respond_with(ResponseTemplate::new(503).set_body_string("Hetzner down"))
            .mount(&server)
            .await;
        let err = delete(&Client::new(), &tunnel(&server), 3)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Hetzner down"), "{err}");
    }

    #[test]
    fn an_unconnected_machine_says_why_it_cannot_buy_a_disk() {
        let t = Tunnel {
            enabled: false,
            platform_api_url: String::new(),
            account_token: String::new(),
            tunnel_id: String::new(),
            host: String::new(),
        };
        assert!(connected(&t).is_err());
    }
}
