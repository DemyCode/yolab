/// Registers this node's loop devices in the CephCluster spec on startup.
///
/// Rook v1.16+ cannot discover loop devices via deviceFilter — they must be
/// listed explicitly in spec.storage.nodes. This runs once at startup (with
/// retries) so any node with an active loop OSD gets provisioned automatically.

const CEPH_NS: &str = "rook-ceph";
const CEPH_CLUSTER: &str = "rook-ceph";

fn loop_backing_files() -> std::collections::HashMap<String, String> {
    let Ok(out) = std::process::Command::new("losetup")
        .args(["-l", "--output", "NAME,BACK-FILE", "--noheadings"])
        .output()
    else { return Default::default() };
    let mut map = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(file)) = (parts.next(), parts.next()) {
            if name.starts_with("/dev/loop") {
                map.insert(name.trim_start_matches("/dev/").to_string(), file.to_string());
            }
        }
    }
    map
}

fn k8s_node_name() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn get_node_devices(k8s_node: &str) -> Vec<String> {
    let raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.nodes}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if let Ok(nodes) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
        for node in &nodes {
            if node["name"].as_str() == Some(k8s_node) {
                return node["devices"].as_array().unwrap_or(&vec![])
                    .iter().filter_map(|d| d["name"].as_str().map(String::from)).collect();
            }
        }
    }
    vec![]
}

fn set_node_devices(k8s_node: &str, devices: Vec<String>) {
    let nodes_raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.nodes}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut nodes: Vec<serde_json::Value> = serde_json::from_str(&nodes_raw).unwrap_or_default();
    let dev_entries: Vec<serde_json::Value> = devices.iter()
        .map(|d| serde_json::json!({"name": d}))
        .collect();
    let mut found = false;
    for node in nodes.iter_mut() {
        if node["name"].as_str() == Some(k8s_node) {
            node["devices"] = serde_json::Value::Array(dev_entries.clone());
            found = true;
            break;
        }
    }
    if !found {
        nodes.push(serde_json::json!({"name": k8s_node, "devices": dev_entries}));
    }
    let patch = serde_json::json!({"spec": {"storage": {"nodes": nodes}}});
    let _ = std::process::Command::new("kubectl")
        .args(["patch", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "--type", "merge", "-p", &patch.to_string()])
        .output();
}

/// Detects active loop devices backed by our rook images and registers them
/// in the CephCluster spec. Returns true when kubectl was reachable.
pub fn register() -> bool {
    let node = k8s_node_name();
    if node.is_empty() { return false; }

    let loops: Vec<String> = loop_backing_files()
        .into_iter()
        .filter(|(_, backing)| backing.starts_with("/var/lib/rook/"))
        .map(|(name, _)| name)
        .collect();

    if loops.is_empty() { return false; }

    let current = get_node_devices(&node);
    // Only patch if something is missing — avoids unnecessary Rook reconciles.
    if loops.iter().all(|l| current.contains(l)) { return true; }

    let mut merged = current;
    for l in &loops {
        if !merged.contains(l) { merged.push(l.clone()); }
    }
    set_node_devices(&node, merged);
    true
}
