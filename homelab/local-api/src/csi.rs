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

use crate::error::Outcome;
use crate::host::Host;

pub const NS: &str = "rook-ceph";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    /// The plugin pod scheduled on this node only.
    ThisNode,
    /// Plugin and provisioner pods on every node.
    AllNodes,
}

/// Best effort by design: a pod that cannot be deleted now is retried by the
/// caller's next tick, and nothing here is destructive — the DaemonSet brings
/// every pod straight back.
pub async fn restart_plugins<H: Host>(host: &H, which: Which) {
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
            .warn_on_err("restart this node's CephFS CSI plugin");
        }
        Which::AllNodes => {
            for app in ["csi-cephfsplugin", "csi-cephfsplugin-provisioner"] {
                let selector = format!("app={app}");
                host.kubectl(&["delete", "pod", "-n", NS, "-l", &selector, "--wait=false"])
                    .await
                    .warn_on_err(format!("restart {app} pods"));
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
        restart_plugins(&host, Which::ThisNode).await;
        assert!(host.ran("kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin --field-selector spec.nodeName="));
        assert!(!host.ran("csi-cephfsplugin-provisioner"));
    }

    #[tokio::test]
    async fn all_nodes_restarts_plugin_and_provisioner() {
        let host = FakeHost::new().ok("kubectl delete pod", "");
        restart_plugins(&host, Which::AllNodes).await;
        assert!(host.ran("-l app=csi-cephfsplugin --wait=false"));
        assert!(host.ran("-l app=csi-cephfsplugin-provisioner --wait=false"));
        assert!(!host.ran("--field-selector"));
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
