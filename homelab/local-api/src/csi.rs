//! Restarting this node's CephFS CSI plugin pod.
//!
//! After THIS node reboots, its plugin pod holds in-memory operation locks from
//! the previous boot, and every pod mounting CephFS fails with "an operation with
//! the given Volume ID … already exists" for ~10 minutes. Only this node's plugin
//! pod is deleted: a whole-DaemonSet restart bounced every other node's live
//! mounts for a problem they did not have. (Was `yolab-csi-recovery.service`; now
//! the once-per-boot `csi-recovery` controller.)

use serde_json::Value;

use crate::exec::CmdError;
use crate::host::Host;

pub const NS: &str = "rook-ceph";

/// Nothing here is destructive — the DaemonSet brings the pod straight back — but
/// a restart that did not happen is reported, never swallowed: csi-recovery
/// records its once-per-boot marker on success, and recording it over a failed
/// delete meant the stale plugin was never restarted at all.
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

/// Whether Rook has created the plugin DaemonSet yet. `Ok(false)` only when the
/// API says it does not exist.
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
