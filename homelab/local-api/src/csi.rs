//! Restarting the CephFS CSI driver's pods. One implementation for the two
//! situations that need it, which used to be two copies with different selectors:
//!
//!   - After THIS node reboots, its plugin pod holds in-memory operation locks
//!     from the previous boot, and every pod mounting CephFS fails with "an
//!     operation with the given Volume ID … already exists" for ~10 minutes.
//!     Only this node's plugin pod is deleted: a whole-DaemonSet restart bounced
//!     every other node's live mounts for a problem they did not have.
//!     (Was `yolab-csi-recovery.service`; now the once-per-boot `csi-recovery`
//!     controller.)
//!   - After a storage recovery REPLACES the filesystem, the plugin and the
//!     provisioner on every node hold state about the old one.

use serde_json::Value;

use crate::exec::CmdError;
use crate::host::Host;

pub const NS: &str = "rook-ceph";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    /// The plugin pod scheduled on this node only.
    ThisNode,
    /// Plugin and provisioner pods on every node.
    AllNodes,
}

/// Nothing here is destructive — the DaemonSet brings every pod straight back —
/// but a restart that did not happen is reported, never swallowed: both callers
/// record "done" on success (csi-recovery's once-per-boot marker, the recovery's
/// step), and recording it over a failed delete meant the stale plugin was never
/// restarted at all. Every delete is attempted; the first failure is returned.
pub async fn restart_plugins<H: Host>(host: &H, which: Which) -> Result<(), CmdError> {
    match which {
        Which::ThisNode => {
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
        Which::AllNodes => {
            let mut first_err = None;
            for app in ["csi-cephfsplugin", "csi-cephfsplugin-provisioner"] {
                let selector = format!("app={app}");
                if let Err(e) = host
                    .kubectl(&["delete", "pod", "-n", NS, "-l", &selector, "--wait=false"])
                    .await
                {
                    tracing::warn!("restart {app} pods: {e}");
                    first_err.get_or_insert(e);
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }
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
    async fn this_node_only_deletes_the_local_plugin_pod() {
        let host = FakeHost::new().ok("kubectl delete pod", "");
        restart_plugins(&host, Which::ThisNode).await.unwrap();
        assert!(host.ran("kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin --field-selector spec.nodeName="));
        assert!(!host.ran("csi-cephfsplugin-provisioner"));
    }

    #[tokio::test]
    async fn all_nodes_restarts_plugin_and_provisioner() {
        let host = FakeHost::new().ok("kubectl delete pod", "");
        restart_plugins(&host, Which::AllNodes).await.unwrap();
        assert!(host.ran("-l app=csi-cephfsplugin --wait=false"));
        assert!(host.ran("-l app=csi-cephfsplugin-provisioner --wait=false"));
        assert!(!host.ran("--field-selector"));
    }

    #[tokio::test]
    async fn a_failed_delete_is_reported_after_every_delete_was_tried() {
        let host = FakeHost::new()
            .fail("kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin ", "etcd timeout")
            .ok("kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin-provisioner", "");
        assert!(restart_plugins(&host, Which::AllNodes).await.is_err());
        assert!(host.ran("app=csi-cephfsplugin-provisioner"), "the second is still tried");

        let local = FakeHost::new().fail("kubectl delete pod", "etcd timeout");
        assert!(restart_plugins(&local, Which::ThisNode).await.is_err());
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
