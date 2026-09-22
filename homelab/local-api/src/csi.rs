
use serde_json::Value;

use crate::exec::CmdError;
use crate::host::Host;

pub const NS: &str = "rook-ceph";

pub async fn restart_local_plugin<H: Host>(host: &H) -> Result<(), CmdError> {
    let selector = format!("spec.nodeName={}", crate::system::hostname());
    host.kubectl(&[
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
    .await
    .map(|_| ())
}

pub async fn plugin_daemonset_exists<H: Host>(host: &H) -> Result<bool, crate::exec::CmdError> {
    let got: Option<Value> = host
        .kubectl_get_opt(&[
            "get",
            "daemonset",
            "csi-cephfsplugin",
            "-n",
            NS,
            "-o",
            "json",
        ])
        .await?;
    Ok(got.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[tokio::test]
    async fn only_the_local_plugin_pod_is_deleted_and_a_failure_is_reported() {
        let host = FakeHost::new().ok("kubectl delete pod", "");
        restart_local_plugin(&host).await.unwrap();
        assert!(host.ran("kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin --field-selector spec.nodeName="));
        assert!(!host.ran("csi-cephfsplugin-provisioner"));

        let failing = FakeHost::new().fail("kubectl delete pod", "etcd timeout");
        assert!(restart_local_plugin(&failing).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_daemonset_is_distinguished_from_an_unreachable_api() {
        let absent = FakeHost::new().fail(
            "kubectl get daemonset csi-cephfsplugin",
            "Error from server (NotFound): daemonsets.apps \"csi-cephfsplugin\" not found",
        );
        assert!(!plugin_daemonset_exists(&absent).await.unwrap());
        let down = FakeHost::new().fail(
            "kubectl get daemonset csi-cephfsplugin",
            "The connection to the server localhost:6443 was refused",
        );
        assert!(plugin_daemonset_exists(&down).await.is_err());
    }
}
