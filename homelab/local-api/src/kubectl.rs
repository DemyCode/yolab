// kube-rs failed to connect to https://[::1]:6443 in IPv6 environments; all
// cluster access goes through kubectl which works correctly.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Every `kubectl` invocation in this file is bounded by this and
/// `kill_on_drop(true)`. Every reconcile loop in the crate — disks, backups,
/// restores, topology — eventually calls through here, on one shared
/// non-concurrent path (`reconcile_tick`'s single await chain), so a `kubectl`
/// that hangs against a briefly-unresponsive apiserver used to wedge all of
/// them at once, forever, with nothing to recover it short of a restart —
/// the same incident class `ceph_cli.rs`'s blanket 30s timeout exists to
/// prevent for `ceph` calls, which this had no equivalent of.
const KUBECTL_TIMEOUT: Duration = Duration::from_secs(60);

async fn run_bounded<F>(what: &str, f: F) -> Result<std::process::Output>
where
    F: std::future::Future<Output = std::io::Result<std::process::Output>>,
{
    tokio::time::timeout(KUBECTL_TIMEOUT, f)
        .await
        .map_err(|_| anyhow::anyhow!("{what} timed out after {}s", KUBECTL_TIMEOUT.as_secs()))?
        .with_context(|| what.to_string())
}

pub async fn run(args: &[&str]) -> Result<String> {
    let out = run_bounded(
        &format!("kubectl {}", args.join(" ")),
        Command::new("kubectl")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        bail!(
            "kubectl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

pub async fn get_json(args: &[&str]) -> Result<Value> {
    let out = run(args).await?;
    serde_json::from_str(&out).context("JSON parse")
}

/// Whether a `kubectl get` failure means "this object does not exist" rather
/// than "the API server did not answer".
///
/// The two must never be confused where the caller maps the result onto a
/// default: NotFound is the genuine first-run state and may be read as empty;
/// an unreachable API server must stay "unknown" so a brief outage is not read
/// as "every disk switched off" (which ends in a drain/purge/wipe — see
/// disks_reconciler::read_desired).
pub fn is_not_found(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("NotFound")
}

// ── Shared apply / secret helpers ─────────────────────────────────────────────
//
// One implementation for the whole crate. Previously auth.rs, backups.rs, and
// apps.rs each carried their own copies (auth.rs even shelled out to `base64 -d`
// on the load path); those all funnel here now.

/// Spawn `kubectl <verb> -f -`, pipe `manifest` to its stdin, and wait —
/// bounded by `KUBECTL_TIMEOUT`, with `kill_on_drop(true)` so a timeout
/// actually kills the child instead of orphaning it still holding the
/// connection open. `apply`/`create`/`replace` are this with one word swapped.
async fn pipe_manifest(verb: &str, manifest: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let work = async {
        let mut child = Command::new("kubectl")
            .args([verb, "-f", "-"])
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn kubectl {verb}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(manifest.as_bytes()).await?;
        }
        child
            .wait_with_output()
            .await
            .with_context(|| format!("kubectl {verb}"))
    };
    let out = tokio::time::timeout(KUBECTL_TIMEOUT, work)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "kubectl {verb} timed out after {}s",
                KUBECTL_TIMEOUT.as_secs()
            )
        })??;
    if !out.status.success() {
        bail!(
            "kubectl {verb}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Pipe a manifest to `kubectl apply -f -`.
pub async fn apply(manifest: &str) -> Result<()> {
    pipe_manifest("apply", manifest).await
}

/// Pipe a manifest to `kubectl create -f -`.
/// Returns Err if the resource already exists (409) or any other failure.
pub async fn create(manifest: &str) -> Result<()> {
    pipe_manifest("create", manifest).await
}

/// Pipe a manifest to `kubectl replace -f -`.
/// Requires `metadata.resourceVersion` in the manifest; fails with 409 if
/// another writer has since modified the resource (optimistic concurrency CAS).
pub async fn replace(manifest: &str) -> Result<()> {
    pipe_manifest("replace", manifest).await
}

/// Read a Secret and return its decoded string data. `None` if missing.
pub async fn get_secret(name: &str, ns: &str) -> Option<std::collections::HashMap<String, String>> {
    let raw = run(&["get", "secret", name, "-n", ns, "-o", "json"])
        .await
        .ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let mut result = std::collections::HashMap::new();
    if let Some(data) = v["data"].as_object() {
        for (k, val) in data {
            let b64 = val.as_str().unwrap_or("");
            let bytes = base64_decode(b64);
            if let Ok(s) = String::from_utf8(bytes) {
                result.insert(k.clone(), s.trim().to_string());
            }
        }
    }
    Some(result)
}

fn base64_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .unwrap_or_default()
}

/// Create or replace an Opaque Secret by generating a kubectl manifest and
/// piping it to `kubectl apply -f -`. Labels are applied to metadata.
pub async fn apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
    labels: &[(&str, &str)],
) -> Result<()> {
    use base64::Engine as _;
    let mut entries = serde_json::Map::new();
    for (k, v) in data {
        let encoded = base64::engine::general_purpose::STANDARD.encode(v.as_bytes());
        entries.insert(k.to_string(), Value::String(encoded));
    }
    let mut label_map = serde_json::Map::new();
    for (k, v) in labels {
        label_map.insert(k.to_string(), Value::String(v.to_string()));
    }
    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": name,
            "namespace": ns,
            "labels": label_map,
        },
        "data": entries,
        "type": "Opaque",
    });
    apply(&manifest.to_string()).await
}

pub async fn get_nodes() -> Result<Vec<Value>> {
    let raw = run(&["get", "nodes", "-o", "json"]).await?;
    let v: Value = serde_json::from_str(&raw).context("parse kubectl get nodes JSON")?;
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

/// Every OTHER node's IPv6 cluster address.
///
/// One copy, because this had grown three: `routers::update::update_all`,
/// `mesh::parse_peer_addresses`, and the reboot fan-out would have been a
/// fourth. Each spelled the same filter slightly differently, and "which nodes
/// are my peers" is not a question that benefits from several opinions.
///
/// IPv6 only: the mesh is v6, so an IPv4 InternalIP here would yield an address
/// nothing in this cluster can actually be reached on.
///
/// Pure, taking the node list rather than fetching it, so the filtering is
/// testable without a cluster.
pub fn peer_ipv6(nodes: &[Value], self_ip: &str) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| {
            n["status"]["addresses"]
                .as_array()?
                .iter()
                .find(|a| {
                    a["type"] == "InternalIP"
                        && a["address"].as_str().is_some_and(|s| s.contains(':'))
                })
                .and_then(|a| a["address"].as_str())
                .map(String::from)
        })
        .filter(|a| a != self_ip)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── peer_ipv6 ────────────────────────────────────────────────────────────

    fn node(ips: &[(&str, &str)]) -> Value {
        serde_json::json!({
            "status": { "addresses": ips.iter()
                .map(|(t, a)| serde_json::json!({"type": t, "address": a}))
                .collect::<Vec<_>>() }
        })
    }

    #[test]
    fn this_node_is_never_its_own_peer() {
        // Fanning out to yourself is at best a wasted request; for the reboot
        // path it would mean this machine rebooting before it told the others.
        let nodes = [
            node(&[("InternalIP", "fd00:cafe::5")]),
            node(&[("InternalIP", "fd00:cafe::6")]),
        ];
        assert_eq!(peer_ipv6(&nodes, "fd00:cafe::5"), vec!["fd00:cafe::6"]);
    }

    #[test]
    fn an_ipv4_internal_ip_is_ignored() {
        // The cluster is reachable over the v6 mesh only, so a v4 address here
        // yields something nothing can actually be reached on.
        let nodes = [node(&[("InternalIP", "10.0.0.7")])];
        assert!(peer_ipv6(&nodes, "fd00:cafe::5").is_empty());
    }

    #[test]
    fn the_hostname_entry_is_not_mistaken_for_an_address() {
        let nodes = [node(&[
            ("Hostname", "node2"),
            ("InternalIP", "fd00:cafe::6"),
        ])];
        assert_eq!(peer_ipv6(&nodes, "fd00:cafe::5"), vec!["fd00:cafe::6"]);
    }

    #[test]
    fn a_single_node_cluster_has_no_peers() {
        let nodes = [node(&[("InternalIP", "fd00:cafe::5")])];
        assert!(peer_ipv6(&nodes, "fd00:cafe::5").is_empty());
    }

    #[test]
    fn every_peer_is_listed_once_and_in_order() {
        let nodes = [
            node(&[("InternalIP", "fd00:cafe::5")]),
            node(&[("InternalIP", "fd00:cafe::6")]),
            node(&[("InternalIP", "fd00:cafe::7")]),
        ];
        assert_eq!(
            peer_ipv6(&nodes, "fd00:cafe::5"),
            vec!["fd00:cafe::6", "fd00:cafe::7"]
        );
    }

    // ── is_not_found ─────────────────────────────────────────────────────────
    //
    // This is the one place in the crate that reads a failed `kubectl get` and
    // has to say whether the resource was absent or the server was unreachable.
    // A missing ConfigMap means "fresh install, safe to treat as empty"; a
    // broken connection must never be read that way.

    #[test]
    fn a_kubectl_not_found_is_a_missing_resource() {
        let e = anyhow::anyhow!(
            "kubectl get configmap yolab-disk-config -n rook-ceph -o json: \
             Error from server (NotFound): configmaps \"yolab-disk-config\" not found"
        );
        assert!(is_not_found(&e));
    }

    #[test]
    fn a_connection_failure_is_not_a_missing_resource() {
        for msg in [
            "kubectl get configmap yolab-disk-config: The connection to the server \
             localhost:6443 was refused - did you specify the right host or port?",
            "kubectl get configmap yolab-disk-config: context deadline exceeded",
            "kubectl get configmap yolab-disk-config: unable to connect to the server: EOF",
        ] {
            let e = anyhow::anyhow!("{msg}");
            assert!(!is_not_found(&e), "{msg}");
        }
    }
}
