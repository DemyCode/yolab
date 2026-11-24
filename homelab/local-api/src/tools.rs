use crate::backup::common::Backend;
use crate::host::MachineHost;
use crate::k8s::Kube;

#[derive(Clone)]
pub struct Tools {
    pub(crate) host: MachineHost,
    pub kube: Kube,
    pub http: crate::http::Client,
}

impl Tools {
    pub fn real() -> Self {
        Tools {
            host: Default::default(),
            kube: Kube::from_environment(),
            http: crate::http::client(),
        }
    }

    pub async fn kube(&self) -> anyhow::Result<kube::Client> {
        self.kube.client().await
    }

    pub async fn backend(&self) -> anyhow::Result<Backend> {
        Ok(Backend {
            kube: self.kube().await?,
            host: self.host.clone(),
        })
    }

    #[cfg(test)]
    pub fn unreachable() -> Self {
        Tools {
            host: Default::default(),
            kube: Kube::with(crate::k8s::testing::unreachable()),
            http: crate::http::client(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_backend_is_built_from_the_kube_client_the_tools_carry() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let (server, client) = crate::k8s::testing::api_server().await;
        Mock::given(method("GET"))
            .and(path("/readyz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let tools = Tools {
            host: Default::default(),
            kube: Kube::with(client),
            http: crate::http::client(),
        };
        let b = tools.backend().await.unwrap();
        assert!(crate::k8s::ready(&b.kube).await);
        assert!(!crate::k8s::ready(&Tools::unreachable().kube().await.unwrap()).await);
    }

    #[tokio::test]
    async fn the_real_tools_reach_nothing_until_they_are_used() {
        let tools = Tools::real();
        assert!(tools.http.get("http://[::1]:9/").build().is_ok());
    }
}
