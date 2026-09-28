use anyhow::{bail, Result};
use kube::Client;

const DAEMONSET_WAIT_ATTEMPTS: u32 = 30;
const DAEMONSET_WAIT_POLL: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn run(client: &Client) -> Result<()> {
    let mut found = false;
    for _ in 0..DAEMONSET_WAIT_ATTEMPTS {
        if matches!(crate::csi::plugin_daemonset_exists(client).await, Ok(true)) {
            found = true;
            break;
        }
        tokio::time::sleep(DAEMONSET_WAIT_POLL).await;
    }
    if !found {
        bail!("csi-cephfsplugin DaemonSet never appeared");
    }
    if let Err(e) = crate::csi::restart_local_plugin(client).await {
        tracing::warn!("csi-recovery: this node's plugin pod was not restarted: {e:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csi::testing::{deleted_plugin_pods, plugin_pods_deleted, DAEMONSET};
    use crate::k8s::testing::{api_server, serve, status};
    use serde_json::json;

    #[tokio::test]
    async fn deletes_this_nodes_plugin_pod_once_the_daemonset_exists() {
        let (server, client) = api_server().await;
        serve(
            &server,
            DAEMONSET,
            200,
            json!({ "metadata": { "name": "csi-cephfsplugin" } }),
        )
        .await;
        plugin_pods_deleted(&server, 200).await;
        run(&client).await.unwrap();
        assert!(deleted_plugin_pods(&server).await);
    }

    #[tokio::test]
    async fn gives_up_when_the_daemonset_never_appears() {
        let (server, client) = api_server().await;
        serve(&server, DAEMONSET, 404, status(404, "NotFound")).await;
        tokio::time::pause();
        let err = run(&client).await.unwrap_err();
        assert!(err.to_string().contains("never appeared"));
        assert!(!deleted_plugin_pods(&server).await);
    }
}
