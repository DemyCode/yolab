
use anyhow::{bail, Result};

use crate::host::Host;

const NS: &str = "rook-ceph";

pub async fn run<H: Host>(host: &H) -> Result<()> {
    let mut found = false;
    for _ in 0..30 {
        if host
            .kubectl(&["get", "daemonset", "csi-cephfsplugin", "-n", NS])
            .await
            .is_ok()
        {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    if !found {
        bail!("csi-cephfsplugin DaemonSet never appeared");
    }

    let hostname = crate::system::hostname();
    let selector = format!("spec.nodeName={hostname}");
    let _ = host
        .kubectl(&[
            "delete",
            "pod",
            "-n",
            NS,
            "-l",
            "app=csi-cephfsplugin",
            "--field-selector",
            &selector,
            "--ignore-not-found",
        ])
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[tokio::test]
    async fn deletes_this_nodes_plugin_pod_once_the_daemonset_exists() {
        let host = FakeHost::new()
            .ok("kubectl get daemonset csi-cephfsplugin -n rook-ceph", "")
            .ok("kubectl delete pod", "");
        run(&host).await.unwrap();
        assert!(host.ran("kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin"));
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_when_the_daemonset_never_appears() {
        let host = FakeHost::new().fail(
            "kubectl get daemonset csi-cephfsplugin -n rook-ceph",
            "not found",
        );
        let err = run(&host).await.unwrap_err();
        assert!(err.to_string().contains("never appeared"));
        assert!(!host.ran("kubectl delete pod"));
    }
}
