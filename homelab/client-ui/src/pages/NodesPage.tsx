import { useEffect, useState } from "react";

interface NodeInfo {
  node_id: string;
  hostname: string;
  platform: string;
  k3s_role: string;
  agent_ip: string;
}

interface ClusterStatus {
  total: number;
  ready: number;
  error?: string;
}

export function NodesPage() {
  const [nodes, setNodes] = useState<NodeInfo[]>([]);
  const [cluster, setCluster] = useState<ClusterStatus | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    Promise.all([
      fetch("/api/nodes").then((r) => r.json()),
      fetch("/api/cluster/status").then((r) => r.json()),
    ])
      .then(([n, c]) => {
        setNodes(Array.isArray(n) ? n : []);
        setCluster(c);
        setLoading(false);
      })
      .catch(() => setLoading(false));
  }, []);

  const installUrl = `${window.location.protocol}//${window.location.host}`;

  return (
    <div>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline", marginBottom: "1rem" }}>
        <h2 style={{ fontSize: "1.2rem", margin: 0 }}>Nodes</h2>
        {cluster && !cluster.error && (
          <span style={{ fontSize: "0.85rem", color: cluster.ready === cluster.total ? "#86efac" : "#f87171" }}>
            {cluster.ready}/{cluster.total} ready
          </span>
        )}
      </div>

      {loading ? (
        <div style={{ color: "#666" }}>Loading…</div>
      ) : nodes.length === 0 ? (
        <div style={{ color: "#666" }}>No nodes found.</div>
      ) : (
        <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.9rem" }}>
          <thead>
            <tr style={{ borderBottom: "1px solid #333" }}>
              {["Hostname", "Platform", "Role", "IP"].map((h) => (
                <th key={h} style={{ textAlign: "left", padding: "0.4rem 0.6rem", color: "#999" }}>{h}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {nodes.map((n) => (
              <tr key={n.node_id} style={{ borderBottom: "1px solid #222" }}>
                <td style={{ padding: "0.4rem 0.6rem" }}>{n.hostname}</td>
                <td style={{ padding: "0.4rem 0.6rem", color: "#aaa" }}>{n.platform}</td>
                <td style={{ padding: "0.4rem 0.6rem", color: "#aaa" }}>{n.k3s_role}</td>
                <td style={{ padding: "0.4rem 0.6rem", fontFamily: "monospace", fontSize: "0.8rem", color: "#aaa" }}>{n.agent_ip}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <details style={{ marginTop: "1.5rem" }}>
        <summary style={{ cursor: "pointer", color: "#999", fontSize: "0.9rem" }}>
          Add another node
        </summary>
        <div style={{ marginTop: "0.75rem", background: "#111", borderRadius: 6, padding: "0.75rem 1rem", fontSize: "0.85rem" }}>
          <div style={{ marginBottom: "0.5rem" }}>Run the installer on the new machine and enter this URL when prompted:</div>
          <div style={{ fontFamily: "monospace", color: "#86efac", display: "flex", alignItems: "center", gap: "0.75rem" }}>
            <span>{installUrl}</span>
            <button
              onClick={() => navigator.clipboard.writeText(installUrl)}
              style={{ padding: "0.2rem 0.6rem", background: "#333", color: "#fff", border: "none", borderRadius: 4, cursor: "pointer", fontSize: "0.8rem" }}
            >
              Copy
            </button>
          </div>
        </div>
      </details>
    </div>
  );
}
