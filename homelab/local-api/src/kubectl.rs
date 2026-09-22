use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

use crate::exec::{self, CmdError};

const KUBECTL_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn run(args: &[&str]) -> Result<String, CmdError> {
    let out = exec::checked("kubectl", args, KUBECTL_TIMEOUT).await?;
    Ok(out.trim().to_string())
}

pub async fn get_json(args: &[&str]) -> Result<Value, CmdError> {
    let out = run(args).await?;
    exec::parse_json(&exec::render("kubectl", args), &out)
}

pub async fn get_opt(args: &[&str]) -> Result<Option<Value>, CmdError> {
    match get_json(args).await {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn is_not_found(e: &impl exec::AsCmdError) -> bool {
    exec::is_not_found(e)
}


async fn pipe_manifest(verb: &str, manifest: &str) -> Result<(), CmdError> {
    exec::with_stdin("kubectl", &[verb, "-f", "-"], manifest, KUBECTL_TIMEOUT)
        .await
        .map(|_| ())
}

pub async fn apply(manifest: &str) -> Result<(), CmdError> {
    pipe_manifest("apply", manifest).await
}

pub async fn create(manifest: &str) -> Result<(), CmdError> {
    pipe_manifest("create", manifest).await
}

pub async fn replace(manifest: &str) -> Result<(), CmdError> {
    pipe_manifest("replace", manifest).await
}

pub async fn get_secret(name: &str, ns: &str) -> Result<Option<HashMap<String, String>>, CmdError> {
    let Some(v) = get_opt(&["get", "secret", name, "-n", ns, "-o", "json"]).await? else {
        return Ok(None);
    };
    let cmd = format!("kubectl get secret {name} -n {ns}");
    let mut result = HashMap::new();
    if let Some(data) = v["data"].as_object() {
        for (k, val) in data {
            let b64 = val
                .as_str()
                .ok_or_else(|| CmdError::parse(&cmd, format!("key {k} is not a string")))?;
            let bytes = base64_decode(b64)
                .map_err(|e| CmdError::parse(&cmd, format!("key {k} is not base64: {e}")))?;
            let s = String::from_utf8(bytes)
                .map_err(|_| CmdError::parse(&cmd, format!("key {k} is not UTF-8")))?;
            result.insert(k.clone(), s.trim().to_string());
        }
    }
    Ok(Some(result))
}

fn base64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s.as_bytes())
}

pub async fn apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
    labels: &[(&str, &str)],
) -> Result<(), CmdError> {
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

pub async fn get_nodes() -> Result<Vec<Value>, CmdError> {
    let v = get_json(&["get", "nodes", "-o", "json"]).await?;
    v["items"]
        .as_array()
        .cloned()
        .ok_or_else(|| CmdError::parse("kubectl get nodes -o json", "no items list"))
}

pub fn peer_ipv6(nodes: &[Value], self_ip: &str) -> Vec<String> {
    nodes
        .iter()
        .filter_map(cluster_ipv6)
        .filter(|a| a != self_ip)
        .collect()
}

fn cluster_ipv6(node: &Value) -> Option<String> {
    node["status"]["addresses"]
        .as_array()?
        .iter()
        .find(|a| {
            a["type"] == "InternalIP" && a["address"].as_str().is_some_and(|s| s.contains(':'))
        })
        .and_then(|a| a["address"].as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;


    fn node(ips: &[(&str, &str)]) -> Value {
        serde_json::json!({
            "status": { "addresses": ips.iter()
                .map(|(t, a)| serde_json::json!({"type": t, "address": a}))
                .collect::<Vec<_>>() }
        })
    }

    #[test]
    fn this_node_is_never_its_own_peer() {
        let nodes = [
            node(&[("InternalIP", "fd00:cafe::5")]),
            node(&[("InternalIP", "fd00:cafe::6")]),
        ];
        assert_eq!(peer_ipv6(&nodes, "fd00:cafe::5"), vec!["fd00:cafe::6"]);
    }

    #[test]
    fn an_ipv4_internal_ip_is_ignored() {
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


    #[test]
    fn a_kubectl_not_found_is_a_missing_resource() {
        let e = CmdError::failed(
            "kubectl get configmap yolab-disk-config -n rook-ceph -o json",
            "Error from server (NotFound): configmaps \"yolab-disk-config\" not found",
        );
        assert!(is_not_found(&e));
    }

    #[test]
    fn a_connection_failure_is_not_a_missing_resource() {
        for msg in [
            "The connection to the server localhost:6443 was refused - did you specify the right host or port?",
            "context deadline exceeded",
            "unable to connect to the server: EOF",
        ] {
            let e = CmdError::failed("kubectl get configmap yolab-disk-config", msg);
            assert!(!is_not_found(&e), "{msg}");
        }
    }

    #[test]
    fn a_plain_anyhow_message_is_never_a_not_found() {
        let e = anyhow::anyhow!("kubectl get configmap x: Error from server (NotFound)");
        assert!(!is_not_found(&e));
    }
}
